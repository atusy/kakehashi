# Kakehashi Custom Methods

Kakehashi extends the Language Server Protocol with a small set of custom methods under the `kakehashi/` namespace. They expose the Tree-sitter syntax tree that kakehashi already maintains to any LSP client, so editors and plugins can build syntax-aware features — structural selection, AST-aware refactoring, long-lived node references — **without bundling Tree-sitter on the client side**.

This document specifies those methods. Where a rule has subtle edge cases, it links to the relevant architecture decision record:

- [Node Reference Protocol](architecture-decisions/node-reference-protocol.md)
- [Lazy Node Identity Tracking](architecture-decisions/lazy-node-identity-tracking.md)

## Method Notation

Each method is marked with its message direction, following the LSP specification's notation:

* :arrow_right: notification, client to server
* :leftwards_arrow_with_hook: request and response, client to server

## Capabilities

These methods are custom and are **not** advertised through `ServerCapabilities`, nor are they gated by a client capability or dynamic registration. A client that knows kakehashi is the server may issue them directly. Position arguments follow the negotiated `positionEncoding`; kakehashi does not advertise one, so per the LSP default `Position.character` is interpreted as a **UTF-16 code unit** offset.

## Basic Structures

The methods reuse the LSP base types [`TextDocumentIdentifier`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocumentIdentifier) and [`Position`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#position), and add the following.

#### NodeInfo

`NodeInfo` is the descriptor returned for a single node.

```typescript
interface NodeInfo {
	/**
	 * A stable, opaque handle to this node, issued lazily on first reference.
	 * The id remains valid across `textDocument/didChange` notifications as
	 * long as the underlying node survives the edit (see Node Identity below).
	 */
	id: string;

	/**
	 * The Tree-sitter node type, for example "fenced_code_block" or "call".
	 * This is an open, grammar-derived string, not an enumerated LSP `*Kind`.
	 */
	type: string;
}
```

#### NodeIdParams

`NodeIdParams` identifies a previously resolved node. It is the parameter type of every id-based method (`parent`, `children`, `text`).

```typescript
interface NodeIdParams {
	/**
	 * The text document the node belongs to. Carried even though the id is
	 * globally unique, so the server can route to the correct per-document
	 * tracker and reject a mismatched (uri, id) pair with `null`.
	 */
	textDocument: TextDocumentIdentifier;

	/**
	 * A node id previously returned by a `kakehashi/node*` method.
	 */
	id: string;
}
```

#### Node Identity

A node `id` is anchored to the node's **start** position. It therefore survives edits to the node's *contents* — the id of a code block is preserved when its body changes — but is invalidated when an edit moves or alters the node's **start**, or changes its type.

#### Null Semantics

Every method returns `null` as a single, uniform "not currently resolvable" signal. A client **cannot** distinguish between an id invalidated by an edit, an id that was never issued for the given document, and a document that has no tracked nodes. All three collapse to `null`, and the correct response is always the same: re-acquire the node with a fresh [`kakehashi/node`](#node-request) request.

## Node Reference Protocol

#### Node Request (:leftwards_arrow_with_hook:)

The node request is sent from the client to the server to resolve a position to the smallest (deepest) node containing it, at a selected injection layer.

_Request_:

* method: 'kakehashi/node'
* params: `NodeParams` defined as follows:

```typescript
interface NodeParams {
	/**
	 * The text document.
	 */
	textDocument: TextDocumentIdentifier;

	/**
	 * The position inside the text document.
	 */
	position: Position;

	/**
	 * Selects which injection layer to resolve in. Defaults to `false`
	 * (the host layer). See the table below.
	 */
	injection?: boolean | integer;
}
```

The `injection` selector operates on the cursor's injection stack `[host, layer₁, …, deepest]`:

| Value | Resolves to |
|---|---|
| `false` (default) or `0` | host layer (layer 0); always succeeds |
| `true` | deepest layer at the cursor, saturating; always succeeds |
| positive `n` (`n ≥ 1`) | exactly layer `n`; `null` if the stack is shallower |
| negative `n` (`n ≤ -1`) | the `n`-th layer from the deepest; `null` if out of range |

_Response_:

* result: `NodeInfo` \| `null`
* error: `null` is returned, rather than an error, when the position lies outside the document, the document is empty, or the requested injection layer does not exist at that position.

A cursor positioned exactly at the end of a non-empty document resolves to the smallest node ending at the document end. Half-open interval and end-of-document rules are specified in the [Node Reference Protocol ADR](architecture-decisions/node-reference-protocol.md).

#### Node Parent Request (:leftwards_arrow_with_hook:)

The node parent request is sent from the client to the server to navigate one step toward the root of the **same language tree**. Navigation does not cross injection boundaries: `parent` on the root of an injected tree returns `null`, not the host node containing the injection. Crossing layers requires a fresh [`kakehashi/node`](#node-request) request.

_Request_:

* method: 'kakehashi/node/parent'
* params: `NodeIdParams`

_Response_:

* result: `NodeInfo` \| `null`
* error: `null` is returned when the id is unknown (re-acquire) or already refers to a root node.

#### Node Children Request (:leftwards_arrow_with_hook:)

The node children request is sent from the client to the server to list a node's immediate children, in document order. Both named and anonymous children are returned.

_Request_:

* method: 'kakehashi/node/children'
* params: `NodeIdParams`

_Response_:

* result: `NodeInfo[]` \| `null`. A node that exists but has no children returns `[]`.
* error: `null` is returned when the id is unknown (re-acquire).

#### Node Text Request (:leftwards_arrow_with_hook:)

The node text request is sent from the client to the server to read a node's **current** text content, sliced from the live document using the node's edit-adjusted byte range.

_Request_:

* method: 'kakehashi/node/text'
* params: `NodeIdParams`

_Response_:

* result: `NodeText` \| `null` defined as follows:

```typescript
interface NodeText {
	/**
	 * The node's current text content, consistent with the document the
	 * client has observed after the latest `didChange`.
	 */
	text: string;
}
```

* error: `null` is returned when the id is not currently valid for the document (re-acquire).

#### Example Message Flow

```jsonc
// 1. Resolve the deepest node at a position.
→ kakehashi/node          { "textDocument": {…}, "position": {…}, "injection": true }
← { "id": "01HX-A", "type": "call" }

// 2. The user edits the document; textDocument/didChange is sent.

// 3. The id survived; its text reflects the edit.
→ kakehashi/node/text     { "textDocument": {…}, "id": "01HX-A" }
← { "text": "foo(updated_arg)" }

// 4. Navigate to the enclosing statement.
→ kakehashi/node/parent   { "textDocument": {…}, "id": "01HX-A" }
← { "id": "01HX-B", "type": "expression_statement" }

// 5. List its children.
→ kakehashi/node/children { "textDocument": {…}, "id": "01HX-B" }
← [ { "id": "01HX-A", "type": "call" } ]
```

## Configuration

#### Effective Configuration Request (:leftwards_arrow_with_hook:)

The effective configuration request is sent from the client to the server to inspect the **merged** configuration currently in effect — after combining `initializationOptions`, user and project `kakehashi.toml` files, and built-in defaults. It is a diagnostic endpoint, marked by the `internal/` path segment.

_Request_:

* method: 'kakehashi/internal/effectiveConfiguration'
* params: `EffectiveConfigurationParams` defined as follows:

```typescript
interface EffectiveConfigurationParams {
	// No parameters.
}
```

_Response_:

* result: `EffectiveConfigurationResult` defined as follows:

```typescript
interface EffectiveConfigurationResult {
	/**
	 * The merged settings, matching the `kakehashi.toml` schema documented
	 * under Configuration in the README.
	 */
	settings: object;
}
```

* error: code and message set in case an exception happens while serializing the configuration.

## Planned Additions

The following are specified in the [Node Reference Protocol ADR](architecture-decisions/node-reference-protocol.md) but are **not yet implemented**. They are listed so clients can plan for them; they are not callable today.

| Planned | Shape | Mirrors Tree-sitter |
|---|---|---|
| `namedOnly` member on `NodeParams` | `namedOnly?: boolean`, default `false` | `named_descendant_for_byte_range` |
| `kakehashi/node/namedChildren` request | params `NodeIdParams`, result `NodeInfo[] \| null` | `named_children()` |

Together they let a client resolve and navigate **named-only** nodes — for example, parity with Neovim's `vim.treesitter.get_node({ include_anonymous = false })` — without filtering anonymous nodes on the client. Until they ship, resolve with [`kakehashi/node`](#node-request) and inspect `type`, or walk `parent`/`children` and skip anonymous nodes client-side.
