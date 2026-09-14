# Share capture text metadata across query consumers

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-14 |
| **Decision-makers** | atusy |

## Context and Problem Statement

`#gsub!` was introduced to resolve dynamic injection languages in HTML and Nix.
The captures protocol exposed static properties but omitted transformed text.
Clients of arbitrary query kinds now need the same text metadata that Neovim
provides, while injection-language resolution should consume that common result.
This extends the text-metadata scope of [captures-protocol](captures-protocol.md);
the rest of that protocol remains in effect.

## Decision Drivers

- Keep one runtime text evaluator for all supported consumers.
- Preserve source ranges and raw node identity independently of display text.
- Preserve existing language-resolution failure and directive-order behavior.

## Considered Options

1. Keep text transformation private to injection-language resolution.
2. Evaluate capture metadata centrally and let consumers choose to read `text`.
3. Replace source content with transformed text for every consumer.

## Decision Outcome

Choose option 2. Query execution returns `#gsub!` output in each capture's
existing metadata object, and dynamic `@injection.language` reads that same
metadata before falling back to directive-adjusted source text. A static
`#set! @capture text` is also readable; an empty string does not fall back.

The metadata evaluator operates on a query match and capture ID, independent of
the query kind and capture name. Failed text evaluation keeps static metadata
and geometry available to captures clients but leaves injection language
unresolved, preserving the existing conservative routing behavior.

No source replacement is performed for injection parsing, bridge virtual
documents, predicates, bindings, or raw-node access. Consumers must explicitly
opt into text metadata, as in Neovim's metadata-aware `get_node_text()`.

### Consequences

- Clients can use transformed labels without recreating Lua-pattern evaluation.
- Full, range, and delta responses carry the same text; content changes can now
  produce metadata-only deltas and larger payloads.
- Lua-pattern subset restrictions remain. `#set! text`/`#gsub!` relative order
  cannot be reconstructed from Rust Tree-sitter's separate property/directive
  lists. This combination remains unsupported: gsub uses source text, and its
  resolved result overrides static text, regardless of source order.

### Confirmation

Unit tests cover arbitrary capture text with unchanged geometry, static/empty
language metadata, unresolved multi-node text, existing ordered runtime language
transforms, and unchanged bridge content. Captures E2E verifies full/range
metadata, unchanged deltas, and refresh after an equal-length edit.

## Pros and Cons of the Options

Option 1 avoids new payload data but forces clients to duplicate transformation
logic. Option 2 reuses the existing metadata schema and keeps source geometry
stable, at the cost of evaluating text for requested captures. Option 3 would
require a general source map and change parser semantics; Neovim's injection
content consumer also uses source ranges rather than transformed text.
