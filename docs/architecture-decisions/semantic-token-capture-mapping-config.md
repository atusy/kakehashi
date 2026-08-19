# Semantic Token Capture Mapping Config

**Related Decisions**:
- [configuration-merging-strategy](configuration-merging-strategy.md)
- [wildcard-config-inheritance](wildcard-config-inheritance.md)

## Context

The top-level `captureMappings.<language>.<query-kind>` schema suggests a
general mapping facility shared by every Tree-sitter query consumer. In
practice, only `highlights` mappings affect behavior: they translate
Tree-sitter captures into LSP semantic token types. The sibling `folds` shape
has no consumer, and raw-capture features such as `kakehashi/captures/full` do
not consult these mappings.

Keeping the setting at the workspace root therefore advertises a broader
contract than kakehashi implements and makes it easy to assume that changing a
mapping changes unrelated capture-based features.

## Decision

The canonical setting is
`features.textDocument/semanticTokens.captureMappings.<language>`. Each
language entry maps capture names directly to semantic token types; there is
no intermediate `highlights` key.

The former top-level `captureMappings.<language>.highlights` spelling remains
accepted during migration and produces a deprecation warning at most once per
session. The never-consumed `folds` field is removed from the schema without a
replacement and has no effect. It follows the same source-specific unknown-key
policy as any other unrecognized field: tolerant sources ignore it, while a
pushed runtime update carrying it is rejected. Its presence does not turn an
otherwise valid legacy layer into a parse error or discard sibling
`highlights` entries.

Configuration layering continues to follow configuration-merging-strategy:
later layers override duplicate capture names while inheriting names they omit.
Within one layer that contains both spellings, the canonical feature-scoped
value wins duplicate names and retains names present only in the legacy shape.
Merge order is inherited mappings, then the legacy spelling (including an
empty root or language clear), then the canonical spelling. Thus a legacy
clear removes the corresponding inherited mappings before canonical entries
from the same layer are added.
An explicitly empty root map clears every language mapping, and an explicitly
empty language map clears that language entry inherited from lower
configuration layers. The `_` mappings still apply during the later wildcard
resolution step, so an empty language entry does not opt that language out of
wildcard mappings.

Wildcard resolution continues to follow wildcard-config-inheritance: `_`
provides defaults for language-specific entries after cross-layer merging.

## Considered Options

### Keep the top-level schema and document its narrow use

Rejected because the namespace would continue to imply that all capture
consumers share the mapping, and the query-kind level would continue to expose
an unused `folds` contract.

### Place the map directly under `features.semanticTokens`

Rejected because feature keys elsewhere mirror LSP method names. Using
`textDocument/semanticTokens` makes ownership explicit and follows the existing
feature configuration convention.

### Remove the former spelling immediately

Rejected because existing file configuration, initialization options, and
runtime configuration can migrate without ambiguity. A warning preserves
compatibility while making the new ownership visible.

## Consequences

### Positive

- The schema communicates that mappings affect semantic tokens only.
- The unused query-kind level disappears from canonical configuration.
- Raw capture protocols remain clearly independent of token presentation.
- Removing `folds` does not introduce a stricter parser policy for the legacy
  wrapper than sibling configuration uses.

### Negative

- Existing users must migrate a nested configuration shape.
- Parsing and merging must temporarily support both spellings.

### Neutral

- Runtime semantic-token resolution still consumes the same effective mapping.
- Wildcard and cross-layer precedence semantics do not change.
